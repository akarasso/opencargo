# opencargo

Registry de packages universel, leger et auto-heberge, ecrit en Rust.

- **Multi-format** : npm, Cargo (crates Rust), OCI / Docker
- **Binaire unique**, ~10 Mo, ~10-30 Mo RAM
- **Zero JVM**, zero GC — SQLite embarque
- **Proxy + cache** : cache transparent vers npmjs.org, crates.io
- **Repos group** : un seul endpoint pour packages prives + publics
- **Promotion de packages** : workflow dev → prod avec audit trail
- **UI web** SolidJS embarquee dans le binaire
- **Metriques Prometheus** integrees

---

## Quickstart

### 1. Build

```bash
cd frontend && pnpm install && pnpm build && cd ..
cargo build --release
```

### 2. Configuration

```bash
cp config.example.toml config.toml
```

### 3. Lancer

```bash
./target/release/opencargo --config config.toml
```

Au premier lancement, un mot de passe admin aleatoire est genere et ecrit dans `data/admin.password`. Consultez les logs ou le fichier pour le recuperer.

```bash
cat data/admin.password
```

Ouvrir `http://localhost:6789/` dans un navigateur, se connecter et changer le mot de passe.

---

## Configuration

### Config minimale

```toml
[server]
bind = "127.0.0.1:6789"
base_url = "http://localhost:6789"
storage_path = "./data/storage"

[database]
url = "sqlite:./data/db/opencargo.db"

[auth]
anonymous_read = true

[auth.admin]
username = "admin"

[[repositories]]
name = "npm-private"
type = "hosted"
format = "npm"
visibility = "private"
```

### Config complete

```toml
[server]
bind = "127.0.0.1:6789"
base_url = "http://localhost:6789"
storage_path = "./data/storage"

[database]
url = "sqlite:./data/db/opencargo.db"

[auth]
anonymous_read = true           # GET sans token autorise
token_prefix = "trg_"           # Prefixe des tokens generes
static_tokens = []              # Tokens fixes (dev/CI)

[auth.admin]
username = "admin"
# password est genere automatiquement au premier lancement
# Pour forcer un mot de passe : password = "mon-mdp"
# En k8s, utiliser la variable d'env OPENCARGO_ADMIN_PASSWORD

[proxy]
default_ttl = "24h"
negative_cache_ttl = "1h"
connect_timeout = "10s"

[cleanup]
enabled = true
prerelease_older_than_days = 90
proxy_cache_older_than_days = 180

# --- Repositories ---

[[repositories]]
name = "npm-private"
type = "hosted"
format = "npm"
visibility = "private"

[[repositories]]
name = "npm-proxy"
type = "proxy"
format = "npm"
visibility = "public"
upstream = "https://registry.npmjs.org"

[[repositories]]
name = "npm-all"
type = "group"
format = "npm"
visibility = "public"
members = ["npm-private", "npm-proxy"]

[[repositories]]
name = "cargo-private"
type = "hosted"
format = "cargo"
visibility = "private"
```

### Variables d'environnement

| Variable | Description |
|----------|-------------|
| `OPENCARGO_CONFIG` | Chemin vers le fichier de config |
| `OPENCARGO_ADMIN_PASSWORD` | Mot de passe admin (prioritaire sur la generation aleatoire) |

---

## Types de repositories

| Type | Description |
|------|-------------|
| **hosted** | Stockage local. C'est la que vous publiez vos packages. |
| **proxy** | Cache transparent vers un registry upstream. Les packages sont telecharges a la demande puis caches. |
| **group** | Agregation de plusieurs repos derriere une URL unique. Resolution dans l'ordre configure. |

---

## Architectures type

### Setup simple (equipe unique)

Un seul repo hosted pour les packages prives, un proxy pour le cache, un group pour tout combiner.

```toml
[[repositories]]
name = "npm-private"
type = "hosted"
format = "npm"

[[repositories]]
name = "npm-proxy"
type = "proxy"
format = "npm"
upstream = "https://registry.npmjs.org"

[[repositories]]
name = "npm-all"
type = "group"
format = "npm"
members = ["npm-private", "npm-proxy"]
```

```ini
# .npmrc — tout le monde utilise le group
@monscope:registry=http://registry:6789/npm-all/
//registry:6789/npm-all/:_authToken=mon-token
```

Pas de promotion, pas de complexite. Les devs publient dans `npm-private`, tout le monde installe depuis `npm-all`.

### Setup avec promotion (dev → prod)

Deux repos hosted : un pour le developpement, un pour la production. Les packages sont promus de l'un a l'autre apres validation.

```toml
[[repositories]]
name = "npm-dev"        # les devs publient ici
type = "hosted"
format = "npm"

[[repositories]]
name = "npm-prod"       # packages valides/promus
type = "hosted"
format = "npm"

[[repositories]]
name = "npm-proxy"
type = "proxy"
format = "npm"
upstream = "https://registry.npmjs.org"

[[repositories]]
name = "npm-all"
type = "group"
format = "npm"
members = ["npm-prod", "npm-dev", "npm-proxy"]
```

**Important** : l'ordre des `members` compte. `npm-prod` est resolu en premier — si un package existe en prod ET en dev, c'est la version prod qui est servie.

```ini
# .npmrc — identique pour les devs et la CI
@monscope:registry=http://registry:6789/npm-all/
//registry:6789/npm-all/:_authToken=mon-token
```

Tout le monde utilise le meme group `npm-all` et le meme `.npmrc`. Le lockfile reste identique entre dev et CI — la promotion ne change pas les URLs.

**Workflow :**

```bash
# 1. Le dev publie dans npm-dev
pnpm publish  # → @monscope/auth-sdk@1.0.0-dev.28 dans npm-dev

# 2. QA valide

# 3. L'admin promeut vers npm-prod
curl -X POST http://registry:6789/api/v1/packages/@monscope/auth-sdk/versions/1.0.0-dev.28/promote \
  -H "Authorization: Bearer admin-token" \
  -H "Content-Type: application/json" \
  -d '{"from": "npm-dev", "to": "npm-prod"}'

# Le tarball n'est pas copie — les deux repos pointent vers le meme fichier.
# Le lockfile ne change pas car tout le monde utilise npm-all.
```

---

## Utilisation npm / pnpm

### Configurer le client

```ini
# .npmrc
@monscope:registry=http://localhost:6789/npm-all/
//localhost:6789/npm-all/:_authToken=mon-token
```

### Publier

```bash
pnpm publish
```

### Installer

```bash
pnpm install @monscope/mon-package
```

### npm login

```bash
npm login --registry http://localhost:6789/npm-all/
```

---

## Utilisation Cargo (crates Rust)

### Configurer Cargo

```toml
# .cargo/config.toml
[registries.private]
index = "sparse+http://localhost:6789/cargo-private/index/"
token = "Bearer mon-token"
```

### Publier

```bash
cargo publish --registry private
```

### Dependre d'une crate privee

```toml
[dependencies]
ma-crate = { version = "0.1", registry = "private" }
```

### Yank / unyank

```bash
curl -X DELETE http://localhost:6789/cargo-private/api/v1/crates/ma-crate/0.1.0/yank \
  -H "Authorization: Bearer mon-token"

curl -X PUT http://localhost:6789/cargo-private/api/v1/crates/ma-crate/0.1.0/unyank \
  -H "Authorization: Bearer mon-token"
```

---

## Utilisation Docker / OCI

opencargo supporte le protocole OCI Distribution Spec v2 pour les images Docker.

### Configurer un repository OCI

Dans `config.toml` ou via l'API admin :

```toml
[[repositories]]
name = "oci-private"
type = "hosted"
format = "oci"
visibility = "private"
```

Ou via l'API :
```bash
curl -X POST http://localhost:6789/api/v1/repositories \
  -H "Authorization: Bearer admin-token" \
  -d '{"name": "oci-private", "type": "hosted", "format": "oci", "visibility": "private"}'
```

### Docker login

```bash
docker login localhost:6789 -u mon-user -p mon-password
```

### Push une image

```bash
docker tag myapp:latest localhost:6789/oci-private/myapp:latest
docker push localhost:6789/oci-private/myapp:latest
```

### Pull une image

```bash
docker pull localhost:6789/oci-private/myapp:latest
```

### Lister les tags

```bash
curl http://localhost:6789/v2/oci-private/myapp/tags/list
```

---

## Authentification

### Mot de passe admin initial

Au premier lancement :
- **Standalone** : un mot de passe aleatoire est genere et ecrit dans `data/admin.password`. Il doit etre change au premier login.
- **Kubernetes** : passer le mot de passe via `OPENCARGO_ADMIN_PASSWORD` (depuis un Secret k8s). Pas de fichier, pas de changement force.

### Utilisateurs et roles

| Role | Lecture | Publication | Promotion | Administration |
|------|---------|-------------|-----------|----------------|
| `admin` | oui | oui | oui | oui |
| `publisher` | oui | oui | non | non |
| `reader` | oui | non | non | non |

### Creer un utilisateur

```bash
curl -X POST http://localhost:6789/api/v1/users \
  -H "Authorization: Bearer admin-token" \
  -H "Content-Type: application/json" \
  -d '{"username": "dev1", "email": "dev1@company.com", "role": "publisher"}'

# Reponse : {"username": "dev1", "password": "aB3kX9...", "role": "publisher"}
# Le mot de passe est genere aleatoirement et retourne UNE SEULE FOIS.
# L'admin ne choisit pas le mot de passe — il le transmet au dev.
```

### Changer son mot de passe

```bash
curl -X PUT http://localhost:6789/api/v1/users/dev1/password \
  -H "Authorization: Bearer dev1-token" \
  -H "Content-Type: application/json" \
  -d '{"current_password": "aB3kX9...", "new_password": "mon-nouveau-mdp"}'
```

### Creer un token API

```bash
curl -X POST http://localhost:6789/api/v1/users/dev1/tokens \
  -H "Authorization: Bearer admin-token" \
  -H "Content-Type: application/json" \
  -d '{"name": "laptop", "expires_in_days": 365}'

# Reponse : {"id": "...", "token": "trg_a1b2c3...", ...}
# Le token brut est retourne UNIQUEMENT a la creation.
```

---

## Interface Web

L'UI est une SPA SolidJS embarquee dans le binaire. Ouvrir `http://localhost:6789/`.

**Public (sans authentification) :**
- Dashboard : stats, packages recents
- Packages : liste avec recherche et filtre par repo
- Detail package : README, versions, commande d'install
- Recherche

**Admin (apres login) :**
- Vue d'ensemble admin
- Gestion des repositories
- Gestion des utilisateurs (creer, modifier, supprimer)
- Gestion des tokens
- Gestion des packages (supprimer, yank)
- Journal d'audit
- Status systeme et metriques

---

## API REST

### Protocole npm

```
GET    /{repo}/@{scope}/{name}                    Metadata du package
GET    /{repo}/@{scope}/{name}/-/{file}.tgz       Telecharger le tarball
PUT    /{repo}/@{scope}/{name}                    Publier une version
GET    /{repo}/-/v1/search?text=query             Recherche
GET    /{repo}/-/package/@{scope}/{name}/dist-tags  Dist-tags
PUT    /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}  Set dist-tag
DELETE /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}  Supprimer dist-tag
PUT    /-/user/org.couchdb.user:{username}        npm login
GET    /-/whoami                                  Utilisateur courant
```

### Protocole Cargo (sparse)

```
GET    /{repo}/index/config.json                  Config du registry
GET    /{repo}/index/{prefix}/{name}              Index d'une crate
PUT    /{repo}/api/v1/crates/new                  Publier une crate
GET    /{repo}/api/v1/crates/{name}/{ver}/download  Telecharger
DELETE /{repo}/api/v1/crates/{name}/{ver}/yank      Yank
PUT    /{repo}/api/v1/crates/{name}/{ver}/unyank    Unyank
```

### Protocole OCI (Docker)

```
GET    /v2/                                         API version check
HEAD   /v2/{repo}/{name}/blobs/{digest}              Verifier si un blob existe
GET    /v2/{repo}/{name}/blobs/{digest}              Telecharger un blob
DELETE /v2/{repo}/{name}/blobs/{digest}              Supprimer un blob
POST   /v2/{repo}/{name}/blobs/uploads/              Initier un upload de blob
PATCH  /v2/{repo}/{name}/blobs/uploads/{uuid}        Upload de chunk
PUT    /v2/{repo}/{name}/blobs/uploads/{uuid}?digest= Completer un upload
GET    /v2/{repo}/{name}/manifests/{reference}       Telecharger un manifest
HEAD   /v2/{repo}/{name}/manifests/{reference}       Verifier un manifest
PUT    /v2/{repo}/{name}/manifests/{reference}       Push un manifest
DELETE /v2/{repo}/{name}/manifests/{reference}       Supprimer un manifest
GET    /v2/{repo}/{name}/tags/list                   Lister les tags
```

### Administration

```
POST   /api/v1/users                              Creer un utilisateur
GET    /api/v1/users                              Lister les utilisateurs
GET    /api/v1/users/{username}                    Detail utilisateur
PUT    /api/v1/users/{username}                    Modifier utilisateur
DELETE /api/v1/users/{username}                    Supprimer utilisateur
PUT    /api/v1/users/{username}/password           Changer le mot de passe
GET    /api/v1/users/{username}/tokens             Lister les tokens
POST   /api/v1/users/{username}/tokens             Creer un token
DELETE /api/v1/users/{username}/tokens/{id}         Revoquer un token
GET    /api/v1/system/audit?page=1&size=50         Journal d'audit
```

### Promotion

```
POST   /api/v1/packages/{name}/versions/{ver}/promote     Promouvoir une version
GET    /api/v1/packages/{name}/versions/{ver}/promotions   Historique de promotion
```

### Frontend API

```
GET    /api/v1/dashboard                           Stats globales
GET    /api/v1/repositories                        Liste des repos
GET    /api/v1/packages?q=&repo=&page=             Liste paginee
GET    /api/v1/packages/{name}                     Detail package
GET    /api/v1/search?q=                           Recherche
```

### Systeme

```
GET    /health/live     Liveness probe
GET    /health/ready    Readiness probe
GET    /metrics         Metriques Prometheus
```

---

## Metriques Prometheus

```
opencargo_http_requests_total{method, path, status}
opencargo_http_request_duration_seconds{method, path}
opencargo_downloads_total{repo, package}
opencargo_publishes_total{repo, package}
opencargo_cache_hits_total{repo}
opencargo_cache_misses_total{repo}
opencargo_storage_bytes{repo}
```

---

## Deploiement

### Docker

```bash
docker build -t opencargo .
docker run -p 6789:6789 \
  -e OPENCARGO_ADMIN_PASSWORD=mon-mdp-secure \
  -v opencargo-data:/data \
  opencargo --config /config/config.toml
```

### Kubernetes (manifests)

```bash
kubectl apply -k k8s/
```

### Helm

```bash
helm install opencargo helm/opencargo/ \
  --set auth.adminPassword=mon-mdp-secure \
  --set ingress.enabled=true \
  --set ingress.host=registry.company.com
```

### Tilt (dev loop)

```bash
tilt up
```

### Mode sidecar CI

opencargo peut etre utilise comme sidecar dans vos pods CI pour cacher les telechargements npm/cargo. Le sidecar fonctionne comme un proxy-cache local : les packages sont telecharges depuis le registry upstream au premier build, puis servis depuis le cache local pour les builds suivants.

Avantages :
- Premier build : temps normal
- Builds suivants : `pnpm install` en ~5-10s au lieu de ~45s
- Zero configuration cote developpeur

Voir le repertoire `k8s/sidecar/` pour les manifests Kubernetes, ainsi que des exemples pour GitHub Actions et GitLab CI.

```bash
# Deployer le sidecar dans k8s
kubectl apply -f k8s/sidecar/configmap.yaml
kubectl apply -f k8s/sidecar/sidecar-deployment.yaml
```

---

## Vulnerability Scanning (OSV.dev)

opencargo peut scanner les dependances de chaque package publie pour detecter les vulnerabilites connues via l'API gratuite [OSV.dev](https://osv.dev/).

### Configuration

```toml
[vuln_scan]
enabled = true
block_on_critical = false  # true = bloquer la publication si une CVE critique est trouvee
```

### Fonctionnement

- Au moment de la publication, les dependances sont extraites du metadata et envoyees a OSV.dev
- Le scan est asynchrone par defaut (ne bloque pas la publication)
- Si `block_on_critical = true`, le scan est synchrone et bloque la publication en cas de CVE critique (score >= 9.0)
- Les resultats sont stockes en base et consultables via l'API

### API

```
GET    /api/v1/packages/{name}/versions/{version}/vulns    Resultats du scan
POST   /api/v1/packages/{name}/versions/{version}/rescan   Re-scanner une version
```

---

## Tests

```bash
# Tous les tests
cargo test

# Par suite
cargo test --test npm_test           # Integration HTTP npm
cargo test --test pnpm_e2e_test      # E2E avec pnpm 10
cargo test --test proxy_test         # Proxy + group (reseau requis)
cargo test --test auth_test          # Auth, users, tokens, promotion
cargo test --test features_test      # UI, metrics, Cargo
cargo test --test promote_test       # Promotion de packages
cargo test --test vuln_test          # Vulnerability scanning (reseau requis)
cargo test --test docker_e2e_test    # Docker/OCI Basic Auth E2E
cargo test --test oci_test           # OCI push/pull/tags
```

---

## License

MIT

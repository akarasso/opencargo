# opencargo

Registry de packages universel, leger et auto-heberge, ecrit en Rust.

- **Multi-format** : npm, Cargo, OCI/Docker, Go modules
- **Binaire unique**, ~10 Mo, ~10-30 Mo RAM
- **Zero JVM**, zero GC — SQLite embarque
- **Proxy + cache** : cache transparent vers npmjs.org, crates.io
- **Repos group** : un seul endpoint pour packages prives + publics
- **Promotion de packages** : workflow dev → prod avec audit trail
- **Permissions granulaires** : par utilisateur × par repository
- **Dependency graph** : suivi des deps + analyse d'impact
- **Vulnerability scanning** : scan automatique via OSV.dev
- **Webhooks** : notifications sur evenements (publish, promote)
- **UI web** SolidJS embarquee dans le binaire
- **Metriques Prometheus** integrees
- **Recherche full-text** (SQLite FTS5)
- **Rate limiting** sur les endpoints sensibles
- **TLS natif** (rustls)

---

## Quickstart

### 1. Build

```bash
make build       # dev
make release     # production
```

Ou manuellement :
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
make dev         # dev (avec logs debug)
make serve       # production
```

Ou manuellement :
```bash
./target/release/opencargo --config config.toml
```

Au premier lancement, un mot de passe admin aleatoire est genere et ecrit dans `data/admin.password`.

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
```

Les repositories, webhooks et permissions se gerent via l'API admin ou l'UI web — pas besoin de les definir dans le fichier de config.

### Config complete

```toml
[server]
bind = "127.0.0.1:6789"
base_url = "http://localhost:6789"
storage_path = "./data/storage"

[server.tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[database]
url = "sqlite:./data/db/opencargo.db"

[auth]
anonymous_read = true
token_prefix = "trg_"
static_tokens = []

[auth.admin]
username = "admin"
# password genere automatiquement au premier lancement
# Pour forcer : password = "mon-mdp"
# En k8s : variable d'env OPENCARGO_ADMIN_PASSWORD

[proxy]
default_ttl = "24h"
negative_cache_ttl = "1h"
connect_timeout = "10s"

[cleanup]
enabled = true
prerelease_older_than_days = 90
proxy_cache_older_than_days = 180

[vuln_scan]
enabled = true
block_on_critical = false

# Repositories (optionnel — seed au premier lancement, ensuite gerable via API)
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

# Webhooks (optionnel — seed, ensuite gerable via API)
[[webhooks]]
url = "https://ci.company.com/hooks"
events = ["package.published", "package.promoted"]
secret = "mon-secret"
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

### Gestion dynamique des repositories

Les repos peuvent etre crees, modifies et supprimes via l'API sans redemarrer le serveur :

```bash
# Creer un repo
curl -X POST http://localhost:6789/api/v1/repositories \
  -H "Authorization: Bearer admin-token" \
  -d '{"name": "npm-dev", "type": "hosted", "format": "npm", "visibility": "private"}'

# Lister les repos
curl http://localhost:6789/api/v1/repositories \
  -H "Authorization: Bearer admin-token"

# Supprimer un repo
curl -X DELETE http://localhost:6789/api/v1/repositories/npm-dev \
  -H "Authorization: Bearer admin-token"
```

---

## Architectures type

### Setup simple (equipe unique)

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
# .npmrc
@monscope:registry=http://registry:6789/npm-all/
//registry:6789/npm-all/:_authToken=mon-token
```

### Setup avec promotion (dev → prod)

```toml
[[repositories]]
name = "npm-dev"
type = "hosted"
format = "npm"

[[repositories]]
name = "npm-prod"
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

L'ordre des `members` compte : `npm-prod` est resolu en premier.

```bash
# Promouvoir un package
curl -X POST http://registry:6789/api/v1/promote/@monscope/auth-sdk/1.0.0-dev.28 \
  -H "Authorization: Bearer admin-token" \
  -d '{"from": "npm-dev", "to": "npm-prod"}'
```

Le tarball n'est pas copie — les deux repos pointent vers le meme fichier. Le lockfile ne change pas car tout le monde utilise `npm-all`.

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

opencargo supporte le protocole OCI Distribution Spec v2.

### Configurer un repository OCI

Via l'API :
```bash
curl -X POST http://localhost:6789/api/v1/repositories \
  -H "Authorization: Bearer admin-token" \
  -d '{"name": "oci-private", "type": "hosted", "format": "oci", "visibility": "private"}'
```

### Docker login

```bash
docker login localhost:6789 -u mon-user -p mon-password
```

Docker utilise Basic Auth — opencargo le supporte nativement avec les memes users/passwords que le reste.

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

> Note : pour utiliser Docker en HTTP (sans TLS), ajoutez `"insecure-registries": ["localhost:6789"]` dans `/etc/docker/daemon.json` et redemarrez Docker. En production, utilisez TLS ou un reverse proxy HTTPS.

---

## Utilisation Go modules

### Configurer un repository Go

```bash
curl -X POST http://localhost:6789/api/v1/repositories \
  -H "Authorization: Bearer admin-token" \
  -d '{"name": "go-private", "type": "hosted", "format": "go", "visibility": "private"}'
```

### Configurer GOPROXY

```bash
export GOPROXY=http://localhost:6789/go-private,direct
export GONOSUMCHECK=mycompany.com/*
```

### Publier un module

```bash
curl -X PUT http://localhost:6789/go-private/mymodule/@v/v1.0.0 \
  -H "Authorization: Bearer mon-token" \
  --data-binary @module.zip
```

### Installer un module

```bash
go get mymodule@v1.0.0
```

---

## Authentification et autorisation

### Mot de passe admin initial

- **Standalone** : mot de passe aleatoire genere → `data/admin.password` → doit etre change au premier login
- **Kubernetes** : `OPENCARGO_ADMIN_PASSWORD` env var depuis un Secret k8s → pas de fichier, pas de changement force

### Roles par defaut

| Role | Lecture | Publication | Promotion | Administration |
|------|---------|-------------|-----------|----------------|
| `admin` | oui | oui | oui | oui |
| `publisher` | oui | oui | non | non |
| `reader` | oui | non | non | non |

### Permissions granulaires

Les permissions peuvent etre definies par utilisateur et par repository, overridant les roles par defaut :

```bash
# Donner le droit de write sur npm-dev mais pas npm-prod
curl -X PUT http://localhost:6789/api/v1/users/dev1/permissions/npm-dev \
  -H "Authorization: Bearer admin-token" \
  -d '{"can_read": true, "can_write": true, "can_delete": false, "can_admin": false}'

# Lister les permissions d'un user
curl http://localhost:6789/api/v1/users/dev1/permissions \
  -H "Authorization: Bearer admin-token"

# Supprimer une permission specifique (retour au role par defaut)
curl -X DELETE http://localhost:6789/api/v1/users/dev1/permissions/npm-dev \
  -H "Authorization: Bearer admin-token"
```

Resolution des permissions :
1. Role `admin` → acces total, toujours
2. Permission specifique dans `user_permissions` → appliquee si presente
3. Sinon, role par defaut (publisher = read+write, reader = read)

### Creer un utilisateur

```bash
curl -X POST http://localhost:6789/api/v1/users \
  -H "Authorization: Bearer admin-token" \
  -d '{"username": "dev1", "email": "dev1@company.com", "role": "publisher"}'

# Reponse : {"username": "dev1", "password": "aB3kX9...", "role": "publisher"}
# Le mot de passe est genere aleatoirement et retourne UNE SEULE FOIS.
```

### Changer son mot de passe

```bash
curl -X PUT http://localhost:6789/api/v1/users/dev1/password \
  -H "Authorization: Bearer dev1-token" \
  -d '{"current_password": "ancien-mdp", "new_password": "nouveau-mdp"}'
```

### Creer un token API

```bash
curl -X POST http://localhost:6789/api/v1/users/dev1/tokens \
  -H "Authorization: Bearer admin-token" \
  -d '{"name": "laptop", "expires_in_days": 365}'

# Reponse : {"id": "...", "token": "trg_a1b2c3...", ...}
# Le token brut est retourne UNIQUEMENT a la creation.
```

---

## Dependency graph

opencargo suit les dependances entre packages et permet l'analyse d'impact.

```bash
# Dependances d'un package
curl http://localhost:6789/api/v1/deps/@trace/httpservice/dependencies

# Qui depend de ce package ?
curl http://localhost:6789/api/v1/deps/@trace/context/dependents

# Analyse d'impact : que se passe-t-il si on supprime cette version ?
curl http://localhost:6789/api/v1/deps/@trace/context/versions/1.0.0/impact \
  -H "Authorization: Bearer admin-token"
```

Les dependances sont extraites automatiquement au moment de la publication (npm: dependencies/devDependencies, Cargo: deps, Go: go.mod).

---

## Webhooks

Les webhooks notifient des systemes externes quand des evenements se produisent.

### Gestion via API

```bash
# Creer un webhook
curl -X POST http://localhost:6789/api/v1/webhooks \
  -H "Authorization: Bearer admin-token" \
  -d '{"url": "https://ci.company.com/hooks", "events": "package.published,package.promoted", "secret": "mon-secret"}'

# Lister les webhooks
curl http://localhost:6789/api/v1/webhooks \
  -H "Authorization: Bearer admin-token"

# Tester un webhook
curl -X POST http://localhost:6789/api/v1/webhooks/1/test \
  -H "Authorization: Bearer admin-token"

# Supprimer
curl -X DELETE http://localhost:6789/api/v1/webhooks/1 \
  -H "Authorization: Bearer admin-token"
```

### Evenements disponibles

| Evenement | Declencheur |
|-----------|-------------|
| `package.published` | Publication d'une nouvelle version |
| `package.promoted` | Promotion d'un package entre repos |
| `*` | Tous les evenements |

### Signature

Si un `secret` est configure, chaque requete webhook inclut un header `X-Webhook-Signature` contenant le HMAC-SHA256 du body.

---

## Vulnerability scanning (OSV.dev)

opencargo scanne les dependances de chaque package publie via l'API gratuite [OSV.dev](https://osv.dev/).

### Configuration

```toml
[vuln_scan]
enabled = true
block_on_critical = false  # true = bloquer la publication si CVE critique
```

### API

```bash
# Resultats du scan
curl http://localhost:6789/api/v1/vulns/@trace/httpclient/1.0.0 \
  -H "Authorization: Bearer mon-token"

# Re-scanner une version
curl -X POST http://localhost:6789/api/v1/vulns/@trace/httpclient/1.0.0/rescan \
  -H "Authorization: Bearer mon-token"
```

---

## Interface Web

SPA SolidJS embarquee dans le binaire. Ouvrir `http://localhost:6789/`.

**Public :**
- Dashboard (stats, packages recents)
- Packages (liste, recherche, filtre par repo)
- Detail package (README, versions, dependances, securite, install command)
- Recherche full-text
- Containers (guide OCI/Docker)
- Go Modules (guide GOPROXY)

**Admin (apres login) :**
- Vue d'ensemble
- Repositories (CRUD)
- Users (CRUD, permissions)
- Packages (promote, yank, delete)
- Webhooks (CRUD)
- Audit log
- System (health, metrics)
- Password change

---

## API REST

### Protocole npm

```
GET    /{repo}/@{scope}/{name}                    Metadata
GET    /{repo}/@{scope}/{name}/-/{file}.tgz       Tarball
PUT    /{repo}/@{scope}/{name}                    Publier
GET    /{repo}/-/v1/search?text=query             Recherche
GET    /{repo}/-/package/@{scope}/{name}/dist-tags  Dist-tags
PUT    /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}  Set dist-tag
DELETE /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}  Delete dist-tag
PUT    /-/user/org.couchdb.user:{username}        npm login
GET    /-/whoami                                  Utilisateur courant
```

### Protocole Cargo (sparse)

```
GET    /{repo}/index/config.json                  Config
GET    /{repo}/index/{prefix}/{name}              Index
PUT    /{repo}/api/v1/crates/new                  Publier
GET    /{repo}/api/v1/crates/{name}/{ver}/download  Telecharger
DELETE /{repo}/api/v1/crates/{name}/{ver}/yank      Yank
PUT    /{repo}/api/v1/crates/{name}/{ver}/unyank    Unyank
```

### Protocole OCI (Docker)

```
GET    /v2/                                         Version check
HEAD   /v2/{repo}/{name}/blobs/{digest}              Head blob
GET    /v2/{repo}/{name}/blobs/{digest}              Get blob
DELETE /v2/{repo}/{name}/blobs/{digest}              Delete blob
POST   /v2/{repo}/{name}/blobs/uploads/              Start upload
PATCH  /v2/{repo}/{name}/blobs/uploads/{uuid}        Upload chunk
PUT    /v2/{repo}/{name}/blobs/uploads/{uuid}?digest= Complete upload
GET    /v2/{repo}/{name}/manifests/{reference}       Get manifest
HEAD   /v2/{repo}/{name}/manifests/{reference}       Head manifest
PUT    /v2/{repo}/{name}/manifests/{reference}       Push manifest
DELETE /v2/{repo}/{name}/manifests/{reference}       Delete manifest
GET    /v2/{repo}/{name}/tags/list                   List tags
```

### Protocole Go (GOPROXY)

```
GET    /{repo}/{module}/@v/list                   Lister les versions
GET    /{repo}/{module}/@v/{version}.info          Info version
GET    /{repo}/{module}/@v/{version}.mod           go.mod
GET    /{repo}/{module}/@v/{version}.zip           Module zip
PUT    /{repo}/{module}/@v/{version}               Publier
```

### Administration

```
POST   /api/v1/repositories                       Creer un repo
GET    /api/v1/repositories                        Lister les repos
GET    /api/v1/repositories/{name}                 Detail repo
PUT    /api/v1/repositories/{name}                 Modifier repo
DELETE /api/v1/repositories/{name}                 Supprimer repo
POST   /api/v1/repositories/{name}/purge-cache     Purger le cache proxy

POST   /api/v1/users                              Creer un utilisateur
GET    /api/v1/users                              Lister les utilisateurs
GET    /api/v1/users/{username}                    Detail utilisateur
PUT    /api/v1/users/{username}                    Modifier utilisateur
DELETE /api/v1/users/{username}                    Supprimer utilisateur
PUT    /api/v1/users/{username}/password           Changer le mot de passe
GET    /api/v1/users/{username}/tokens             Lister les tokens
POST   /api/v1/users/{username}/tokens             Creer un token
DELETE /api/v1/users/{username}/tokens/{id}        Revoquer un token
GET    /api/v1/users/{username}/permissions         Lister les permissions
PUT    /api/v1/users/{username}/permissions/{repo}  Set permissions
DELETE /api/v1/users/{username}/permissions/{repo}  Supprimer permissions

GET    /api/v1/webhooks                            Lister les webhooks
POST   /api/v1/webhooks                            Creer un webhook
PUT    /api/v1/webhooks/{id}                       Modifier un webhook
DELETE /api/v1/webhooks/{id}                       Supprimer un webhook
POST   /api/v1/webhooks/{id}/test                  Tester un webhook

GET    /api/v1/system/audit?page=1&size=50         Journal d'audit
```

### Promotion

```
POST   /api/v1/promote/@{scope}/{name}/{version}              Promouvoir (scoped)
POST   /api/v1/promote/{name}/{version}                       Promouvoir (unscoped)
GET    /api/v1/promotions/@{scope}/{name}/{version}            Historique (scoped)
GET    /api/v1/promotions/{name}/{version}                     Historique (unscoped)
```

### Dependency graph

```
GET    /api/v1/deps/@{scope}/{name}/dependencies               Dependances
GET    /api/v1/deps/{name}/dependencies                        Dependances (unscoped)
GET    /api/v1/deps/@{scope}/{name}/dependents                 Dependants
GET    /api/v1/deps/{name}/dependents                          Dependants (unscoped)
GET    /api/v1/deps/@{scope}/{name}/versions/{ver}/impact      Impact analysis
GET    /api/v1/deps/{name}/versions/{ver}/impact               Impact analysis (unscoped)
```

### Vulnerability scanning

```
GET    /api/v1/vulns/@{scope}/{name}/{version}                 Resultats scan
POST   /api/v1/vulns/@{scope}/{name}/{version}/rescan          Re-scanner
GET    /api/v1/vulns/{name}/{version}                          Resultats (unscoped)
POST   /api/v1/vulns/{name}/{version}/rescan                   Re-scanner (unscoped)
```

### Frontend API

```
GET    /api/v1/dashboard                           Stats globales
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

### Kubernetes

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

### TLS natif

```toml
[server.tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
```

Si configure, opencargo sert en HTTPS directement via rustls. En k8s, il est plus courant de terminer le TLS a l'Ingress.

### Tilt (dev loop)

```bash
tilt up
```

### Mode sidecar CI

opencargo peut tourner comme sidecar dans les pods CI pour cacher les telechargements. Voir `k8s/sidecar/` pour les manifests et exemples GitHub Actions / GitLab CI.

---

## Makefile

```bash
make help                # Toutes les commandes disponibles
make build               # Build frontend + Rust (dev)
make release             # Build en mode release
make dev                 # Lancer en dev avec logs
make test                # Tous les tests
make test-quick          # Tests sans reseau
make check               # Lint + tests (CI local)
make docker              # Build image Docker
make deploy              # Deployer sur k8s
make helm-install        # Deployer avec Helm
make sidecar-deploy      # Deployer le sidecar CI
make publish-trace       # Publier les @trace/* pour tester
make clean               # Nettoyer
make reset-db            # Reset la DB
```

---

## Tests

```bash
# Via Makefile
make test                # Tous les tests
make test-quick          # Sans reseau
make test-docker         # Docker/OCI
make test-e2e            # E2E complets

# Par suite
cargo test --test npm_test           # Integration npm
cargo test --test pnpm_e2e_test      # E2E pnpm 10
cargo test --test proxy_test         # Proxy + group (reseau)
cargo test --test auth_test          # Auth, users, tokens, rate limit
cargo test --test features_test      # UI, metrics, Cargo
cargo test --test promote_test       # Promotion de packages
cargo test --test oci_test           # OCI push/pull/tags
cargo test --test docker_e2e_test    # Docker Basic Auth E2E
cargo test --test go_test            # Go modules
cargo test --test deps_test          # Dependency graph
cargo test --test webhook_test       # Webhooks
cargo test --test vuln_test          # Vulnerability scanning (reseau)
cargo test --test tls_test           # TLS natif
cargo test --test e2e_scoped_test    # E2E packages scoped
cargo test --test permissions_test   # Permissions granulaires
```

---

## License

MIT

# opencargo — Makefile
# Usage: make help

.PHONY: help build dev test test-quick test-s3 test-load test-network test-docker test-e2e test-e2e-cargo test-e2e-go test-e2e-docker test-e2e-maven test-e2e-nuget test-e2e-import clean frontend release docker deploy undeploy logs publish lint fmt bench bench-smoke

# Load .env if it exists
-include .env
export

# Config
NAMESPACE ?= opencargo
GHCR_REGISTRY ?= ghcr.io/akarasso
REGISTRY ?= $(GHCR_REGISTRY)/opencargo
TAG ?= latest
CONFIG ?= config.toml

help: ## Afficher cette aide
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------------------
# Dev local
# ---------------------------------------------------------------------------

frontend: ## Build le frontend SolidJS
	cd frontend && pnpm install && pnpm build

build: frontend ## Build le projet complet (frontend + Rust)
	touch src/web/mod.rs
	cargo build

release: frontend ## Build en mode release
	touch src/web/mod.rs
	cargo build --release

dev: frontend ## Lancer en mode dev (avec logs)
	touch src/web/mod.rs
	RUST_LOG=opencargo=debug,tower_http=debug cargo run -- --config $(CONFIG)

serve: ## Lancer en mode release
	./target/release/opencargo --config $(CONFIG)

tilt: ## Lancer avec Tilt (hot reload)
	tilt up

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

test: ## Lancer tous les tests
	cargo test

test-quick: ## Tests rapides (sans réseau ni client externe)
	cargo test --test npm_test --test npm_proxy_test --test npm_memory_test --test cargo_test --test cargo_proxy_test \
		--test go_test --test go_proxy_test --test oci_test --test oci_nested_test --test oci_proxy_test \
		--test vuln_test --test group_resolver_test --test auth_test --test features_test \
		--test promote_test --test permissions_test --test policy_test --test publish_limits_test \
		--test pypi_test --test pypi_store_test --test pypi_proxy_test \
		--test maven_test --test maven_proxy_test \
		--test nuget_test --test nuget_feed_store_test --test nuget_proxy_test --test nuget_group_test \
		--test sso_test --test sso_store_test \
		--test mcp_test --test mcp_sync_test --test mcp_probe_test --test mcp_policy_test --test mcp_client_test --test mcp_admin_test \
		--test import_test --test import_managers_test --test import_oci_test \
		--test instance_lease_test --test storage_cli_test --test shutdown_test --test backup_test \
		--test write_amplification_test --test bench_smoke_test --test search_cache_test

test-s3: ## Toute la suite sur S3 (MinIO en conteneur)
	scripts/test-s3.sh

bench: release ## Mesures de consommation, tous les scénarios (BENCH_ARGS pour les options)
	scripts/bench.sh $(BENCH_ARGS)

bench-smoke: ## Le scénario minimal du harnais de mesure (sans réseau ni Docker)
	scripts/bench.sh --smoke

test-load: ## Tests de charge : writer policy (5 000 evenements a 500/s, ~16 s), memo NuGet
	cargo test --lib burst_over_cold_rate_drops_nothing -- --ignored
	cargo test --test nuget_group_test the_registration_memo

test-network: ## Tests contre npmjs.org / osv.dev (OPENCARGO_NETWORK_TESTS=1)
	OPENCARGO_NETWORK_TESTS=1 cargo test --test proxy_test --test vuln_test

test-docker: ## Tests Docker/OCI (HTTP, sans client docker)
	cargo test --test oci_test --test oci_nested_test --test oci_proxy_test --test docker_e2e_test

test-e2e: ## Tests E2E avec les vrais clients (pnpm, cargo, go, docker, mvn, gradle requis)
	OPENCARGO_E2E_REQUIRE=1 cargo test --test pnpm_e2e_test --test e2e_scoped_test \
		--test cargo_e2e_test --test go_e2e_test --test docker_cli_e2e_test --test maven_e2e_test \
		--test mcp_e2e_test --test e2e_shutdown_test

test-e2e-cargo: ## E2E cargo (client cargo requis, ou CARGO_BIN)
	OPENCARGO_E2E_REQUIRE=1 cargo test --test cargo_e2e_test

test-e2e-go: ## E2E go (client go requis, ou GO_BIN)
	OPENCARGO_E2E_REQUIRE=1 cargo test --test go_e2e_test

test-e2e-docker: ## E2E docker CLI (client docker requis, ou DOCKER_BIN)
	OPENCARGO_E2E_REQUIRE=1 cargo test --test docker_cli_e2e_test

test-e2e-maven: ## E2E mvn et gradle (MVN_BIN, GRADLE_BIN pour un Gradle >= 8)
	OPENCARGO_E2E_REQUIRE=1 cargo test --test maven_e2e_test

test-e2e-nuget: ## E2E dotnet (DOTNET_BIN, ou scripts/dotnet-in-docker)
	DOTNET_BIN=$${DOTNET_BIN:-$(CURDIR)/scripts/dotnet-in-docker} OPENCARGO_E2E_REQUIRE=1 cargo test --test nuget_e2e_test dotnet

test-e2e-import: ## E2E import contre Verdaccio, Nexus et registry:2 en conteneurs (docker requis)
	OPENCARGO_E2E_CONTAINERS=1 OPENCARGO_E2E_REQUIRE=1 cargo test --test import_e2e_test

lint: ## Lancer clippy
	cargo clippy -- -D warnings

fmt: ## Formater le code
	cargo fmt
	cd frontend && pnpm exec prettier --write src/

check: lint test ## Lint + tests (CI local)

# ---------------------------------------------------------------------------
# Docker
# ---------------------------------------------------------------------------

docker: frontend ## Build l'image Docker
	docker build -t $(REGISTRY):$(TAG) .

docker-run: ## Lancer via Docker
	docker run -p 6789:6789 \
		-e OPENCARGO_ADMIN_PASSWORD=admin \
		-v opencargo-data:/data \
		$(REGISTRY):$(TAG) --config /config/config.toml

docker-login: ## Login au registry GHCR
	@echo $(GITHUB_TOKEN) | docker login ghcr.io -u akarasso --password-stdin

docker-push: docker-login ## Push l'image Docker sur GHCR
	docker push $(REGISTRY):$(TAG)

# ---------------------------------------------------------------------------
# Kubernetes
# ---------------------------------------------------------------------------

deploy: ## Deployer sur k8s (namespace opencargo)
	kubectl create namespace $(NAMESPACE) --dry-run=client -o yaml | kubectl apply -f -
	kubectl apply -k k8s/ -n $(NAMESPACE)
	@echo "---"
	@echo "Deploye dans le namespace $(NAMESPACE)"
	@echo "Attendre que le pod soit ready:"
	@echo "  kubectl -n $(NAMESPACE) wait --for=condition=ready pod -l app=opencargo --timeout=120s"
	@echo "Port-forward:"
	@echo "  kubectl -n $(NAMESPACE) port-forward svc/opencargo 6789:6789"

undeploy: ## Supprimer le deploiement k8s
	kubectl delete -k k8s/ -n $(NAMESPACE) --ignore-not-found
	@echo "Supprime du namespace $(NAMESPACE)"

helm-install: ## Deployer avec Helm
	helm upgrade --install opencargo helm/opencargo/ \
		--namespace $(NAMESPACE) --create-namespace \
		--set ingress.enabled=false

helm-uninstall: ## Supprimer le deploiement Helm
	helm uninstall opencargo --namespace $(NAMESPACE)

logs: ## Voir les logs du pod k8s
	kubectl -n $(NAMESPACE) logs -f -l app=opencargo

status: ## Status du deploiement k8s
	@echo "=== Pods ==="
	@kubectl -n $(NAMESPACE) get pods -l app=opencargo
	@echo ""
	@echo "=== Services ==="
	@kubectl -n $(NAMESPACE) get svc
	@echo ""
	@echo "=== PVC ==="
	@kubectl -n $(NAMESPACE) get pvc

port-forward: ## Port-forward le service k8s sur localhost:6789
	kubectl -n $(NAMESPACE) port-forward svc/opencargo 6789:6789

# ---------------------------------------------------------------------------
# Sidecar CI
# ---------------------------------------------------------------------------

sidecar-deploy: ## Deployer le sidecar cache CI sur k8s
	kubectl create namespace $(NAMESPACE) --dry-run=client -o yaml | kubectl apply -f -
	kubectl apply -f k8s/sidecar/configmap.yaml -n $(NAMESPACE)
	kubectl apply -f k8s/sidecar/sidecar-deployment.yaml -n $(NAMESPACE)
	@echo "Sidecar CI deploye dans $(NAMESPACE)"

sidecar-undeploy: ## Supprimer le sidecar CI
	kubectl delete -f k8s/sidecar/sidecar-deployment.yaml -n $(NAMESPACE) --ignore-not-found
	kubectl delete -f k8s/sidecar/configmap.yaml -n $(NAMESPACE) --ignore-not-found

# ---------------------------------------------------------------------------
# Maintenance
# ---------------------------------------------------------------------------

clean: ## Nettoyer les fichiers generes
	cargo clean
	rm -rf frontend/dist frontend/node_modules
	rm -rf data/

reset-db: ## Supprimer la base de donnees (reset complet)
	rm -f data/db/opencargo.db data/db/opencargo.db-wal data/db/opencargo.db-shm
	rm -f data/admin.password
	@echo "Base de donnees supprimee. Relancez le serveur pour reinitialiser."

migrate: ## Appliquer les migrations DB
	cargo run -- --config $(CONFIG) migrate

validate-config: ## Valider le fichier de config
	cargo run -- validate-config $(CONFIG)

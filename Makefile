# opencargo — Makefile
# Usage: make help

.PHONY: help build dev test clean frontend release docker deploy undeploy logs publish lint fmt

# Config
NAMESPACE ?= opencargo
REGISTRY ?= opencargo
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

test-quick: ## Tests rapides (sans réseau)
	cargo test --test npm_test --test pnpm_e2e_test --test auth_test --test features_test --test promote_test --test permissions_test

test-network: ## Tests nécessitant le réseau
	cargo test --test proxy_test --test vuln_test

test-docker: ## Tests Docker/OCI
	cargo test --test oci_test --test docker_e2e_test

test-e2e: ## Tests E2E complets
	cargo test --test pnpm_e2e_test --test e2e_scoped_test --test docker_e2e_test

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

docker-push: ## Push l'image Docker
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
# Publish des packages @trace (test)
# ---------------------------------------------------------------------------

publish-trace: ## Publier les 4 packages @trace/* sur le registry local
	@echo "Publication des packages @trace/* sur http://localhost:6789/npm-private/"
	@for pkg in package-context package-logger package-httpclient package-httpservice; do \
		echo "=== $$pkg ==="; \
		TMPDIR=$$(mktemp -d); \
		cp -r ../mono-toolbox/packages/$$pkg/* "$$TMPDIR/"; \
		if [ -d "../mono-toolbox/packages/$$pkg/node_modules" ]; then \
			cp -r ../mono-toolbox/packages/$$pkg/node_modules "$$TMPDIR/"; \
		fi; \
		echo '@trace:registry=http://localhost:6789/npm-private/\n//localhost:6789/npm-private/:_authToken=test-token' > "$$TMPDIR/.npmrc"; \
		cd "$$TMPDIR" && pnpm publish --no-git-checks 2>&1 | tail -3; \
		rm -rf "$$TMPDIR"; \
	done

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

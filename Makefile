.PHONY: all check build test test-smoke lint fmt ci dev play crawl play-coffee crawl-bearing clean help

# --- Config ---

CARGO       := cargo
CURL        := curl -s
JQ          := jq .
PORT        := 3000
BASE        := http://localhost:$(PORT)

# Default Edinburgh coordinates for smoke tests
LAT         := 55.9399
LON         := -3.1779
RADIUS      := 500

# --- Primary targets ---

all: check lint test ## Run checks, lint, and tests (the quick loop)

ci: fmt-check lint test build ## Run everything CI runs (mirror of GitHub Actions)

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# --- Build & check ---

check: ## Type-check without building (fast feedback)
	$(CARGO) check --all-targets

build: ## Full debug build
	$(CARGO) build --all-targets

# --- Testing ---

test: ## Run all tests (unit + integration, parallel)
	$(CARGO) test --all-targets

test-unit: ## Run unit tests only
	$(CARGO) test --lib

test-integration: ## Run integration tests only
	$(CARGO) test --test integration

test-smoke: ## Run smoke tests against real Geoapify (requires .env)
	$(CARGO) test --test smoke -- --ignored

# --- Lint & format ---

lint: ## Run clippy with warnings-as-errors
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## Format all code in-place
	$(CARGO) fmt --all

fmt-check: ## Check formatting (no changes, CI-safe)
	$(CARGO) fmt --all -- --check

# --- Local dev server ---

dev: ## Start the local dev server (requires .env with GEOAPIFY_API_KEY + Redis)
	$(CARGO) run --example devserver

redis-up: ## Start a local Redis container for room storage
	docker compose up -d redis

redis-down: ## Stop the local Redis container
	docker compose down

redis-cli: ## Open redis-cli connected to the local Redis
	docker compose exec redis redis-cli

# --- Smoke tests (requires dev server running) ---

play: ## Hit /api/play with default Edinburgh coords
	@$(CURL) $(BASE)/api/play \
		-d '{"latitude":$(LAT),"longitude":$(LON),"radius":$(RADIUS)}' | $(JQ)

crawl: ## Hit /api/crawl for a 3-pub crawl
	@$(CURL) $(BASE)/api/crawl \
		-d '{"latitude":$(LAT),"longitude":$(LON),"radius":$(RADIUS),"num_pubs":3}' | $(JQ)

play-coffee: ## Hit /api/play in non-alcohol mode
	@$(CURL) $(BASE)/api/play \
		-d '{"latitude":$(LAT),"longitude":$(LON),"radius":$(RADIUS),"mode":"nonalcohol"}' | $(JQ)

crawl-bearing: ## Hit /api/crawl with a bearing filter (east)
	@$(CURL) $(BASE)/api/crawl \
		-d '{"latitude":$(LAT),"longitude":$(LON),"radius":$(RADIUS),"num_pubs":2,"bearing":90.0}' | $(JQ)

# --- Cleanup ---

clean: ## Remove build artifacts
	$(CARGO) clean

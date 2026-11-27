.PHONY: help run build dist test coverage lint fmt check check-rust check-py deny release clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-10s\033[0m %s\n", $$1, $$2}'

run: ## Run joust (release build)
	cargo run --release

build: ## Build a release binary (target/release/joust)
	cargo build --release

dist: ## Package a release archive for this machine into dist/
	scripts/package.sh

test: ## Run the test suite
	cargo test

coverage: ## Line coverage of non-test code (needs cargo-llvm-cov)
	cargo llvm-cov --lcov --output-path target/coverage.lcov
	python3 scripts/coverage.py target/coverage.lcov --min 90

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt
	uv run ruff format

check: check-rust check-py ## Everything CI checks except coverage and cargo-deny

check-rust: ## Rust formatting, lints and tests
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

check-py: ## Lint, type-check and test the Python scripts, and check their pins (needs uv)
	uv run ruff check
	uv run ruff format --check
	uv run ty check
	uv run pytest -q
	uv run scripts/check_pins.py

deny: ## Dependency advisories, licenses, bans and sources (needs cargo-deny)
	cargo deny --locked check

release: ## Bump the version, roll CHANGELOG.md, commit and tag: make release VERSION=x.y.z
	@test -n "$(VERSION)" || { echo "usage: make release VERSION=x.y.z [DRY_RUN=1]"; exit 1; }
	uv run scripts/release.py prepare $(VERSION) $(if $(DRY_RUN),--dry-run)

clean: ## Remove build artifacts
	cargo clean
	rm -rf dist

.PHONY: help run build test coverage lint fmt check clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-10s\033[0m %s\n", $$1, $$2}'

run: ## Run joust (release build)
	cargo run --release

build: ## Build a release binary (target/release/joust)
	cargo build --release

test: ## Run the test suite
	cargo test

coverage: ## Line coverage of non-test code (needs cargo-llvm-cov)
	cargo llvm-cov --lcov --output-path target/coverage.lcov
	python3 scripts/coverage.py target/coverage.lcov --min 90

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt

check: ## Formatting, lints and tests (what CI would run)
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

clean: ## Remove build artifacts
	cargo clean

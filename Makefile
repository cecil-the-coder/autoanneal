.PHONY: check test

check:
	CARGO_BUILD_JOBS=1 cargo check --all-targets
test:
	CARGO_BUILD_JOBS=1 cargo test

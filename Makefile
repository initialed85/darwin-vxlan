.PHONY: test coverage coverage-open install-coverage-tool build release

build:
	cargo build

release:
	cargo build --release

test:
	cargo test --features vmnet-mock

# Prints a line-by-line coverage summary to the terminal.
# Requires: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
coverage:
	cargo llvm-cov --features vmnet-mock --summary-only

# Same as above but opens an HTML report in the browser.
coverage-open:
	cargo llvm-cov --html --open

install-coverage-tool:
	rustup component add llvm-tools-preview
	cargo install cargo-llvm-cov

.PHONY: build release test fmt fmt-check clippy lint clean install setup

build:
	cargo build

release:
	cargo build --release

test:
	cargo test --all-features

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --all-targets --all-features

lint: fmt-check clippy

clean:
	cargo clean

install:
	cargo install --path .

setup:
	git config core.hooksPath .githooks
	@echo "Git hooks configured."

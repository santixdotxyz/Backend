.PHONY: all build test fmt lint clean run

all: fmt lint test build

build:
	cargo build --release
	@ls -lh target/release/saintix-indexer | awk '{print "binary: " $$5}'

## No network. Every test here works against ScVals constructed in the test itself, so the suite
## runs on a machine with no RPC reachable and gives the same answer every time.
##
## What it does NOT test is the node's wire format, which no offline test can. `scripts/demo.sh`
## covers that end: it runs this binary against the treasury it has just driven, and a decoder
## that disagreed with the real RPC would fail there.
test:
	cargo test

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

clean:
	cargo clean

## Point it at a deployed treasury:
##   make run ARGS="report --contract CXXX... --network testnet"
run:
	cargo run --release -- $(ARGS)

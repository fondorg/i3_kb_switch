PREFIX     ?= $(HOME)/.local/bin
BIN        ?= $(PREFIX)/i3_kb_switch
MUSL_TARGET := x86_64-unknown-linux-musl
STATIC_BIN  := target/$(MUSL_TARGET)/release/i3_kb_switch
DAEMON_LOG  := $(HOME)/.local/state/i3_kb_switch/daemon.log
CARGO_PATH  := PATH=$(HOME)/.cargo/bin:$$PATH

.DEFAULT_GOAL := help

.PHONY: help build release static test lint fmt install tmux-setup restart stop status clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[01m%-10s\033[0m %s\n", $$1, $$2}'

build: ## Debug build (host toolchain)
	cargo build

release: ## Release build (host toolchain)
	cargo build --release

static: ## Static release build (musl; uses rustup toolchain)
	$(CARGO_PATH) cargo build --release --target $(MUSL_TARGET)

test: ## Run test suite
	cargo test

lint: ## Run clippy and rustfmt checks
	cargo clippy --all-targets
	cargo fmt --check

install: static ## Install static binary to ~/.local/bin, restart the daemon, wire tmux hooks
	@pgrep -x i3_kb_switch >/dev/null && pkill -x i3_kb_switch || true
	@for i in $$(seq 1 20); do pgrep -x i3_kb_switch >/dev/null || break; sleep 0.1; done
	install -m 0755 $(STATIC_BIN) $(PREFIX)/i3_kb_switch
	@$(MAKE) --no-print-directory restart
	@$(MAKE) --no-print-directory tmux-setup BIN=$(PREFIX)/i3_kb_switch

tmux-setup: ## Wire pane hooks into an existing tmux config (idempotent; never creates one)
	@bash scripts/tmux-setup.sh "$(BIN)"

restart: ## (Re)start the daemon from the installed binary
	@pgrep -x i3_kb_switch >/dev/null && pkill -x i3_kb_switch || true
	@for i in $$(seq 1 20); do pgrep -x i3_kb_switch >/dev/null || break; sleep 0.1; done
	@mkdir -p $$(dirname $(DAEMON_LOG))
	@setsid nohup $(PREFIX)/i3_kb_switch >>$(DAEMON_LOG) 2>&1 </dev/null &
	@sleep 0.5
	@pgrep -x i3_kb_switch >/dev/null && echo "daemon running: $$(pgrep -x i3_kb_switch)" || (echo "ERROR: daemon failed to start, see $(DAEMON_LOG)"; exit 1)

stop: ## Stop the daemon (graceful: flushes state on SIGTERM)
	@pgrep -x i3_kb_switch >/dev/null && pkill -x i3_kb_switch && echo "stopped" || echo "not running"

status: ## Show daemon status
	@pgrep -ax i3_kb_switch || echo "not running"

clean: ## Remove build artifacts
	cargo clean

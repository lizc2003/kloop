# kloop — build / test / install
#
# 仓库根不是 cargo workspace 根,workspace 在 rust/ 下,所以每个目标都先 cd 进去。
# 覆盖点:PREFIX / BINDIR(装到哪)、KLOOP_DIR(私有状态根,配置就在它下面)。
#
#   make                 release 构建
#   make test            全量测试
#   make check           fmt --check + clippy -D warnings + test + parity(提交前的门禁,本地唯一一道)
#   make install         装二进制;没有配置时顺带铺一份 config-demo
#   make help            列出所有目标

CARGO ?= cargo
RUST_DIR := rust
BIN := kloop
RELEASE_BIN := $(RUST_DIR)/target/release/$(BIN)

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin

KLOOP_DIR ?= $(HOME)/.kloop
CONFIG := $(KLOOP_DIR)/config.toml
CONFIG_DEMO := config/config-demo.toml

.DEFAULT_GOAL := all
.PHONY: all build debug test fmt fmt-check clippy check parity mock install install-config uninstall clean help

all: build

build:
	cd $(RUST_DIR) && $(CARGO) build --locked --release

debug:
	cd $(RUST_DIR) && $(CARGO) build --locked

test:
	cd $(RUST_DIR) && $(CARGO) test --locked --workspace

fmt:
	cd $(RUST_DIR) && $(CARGO) fmt --all

fmt-check:
	cd $(RUST_DIR) && $(CARGO) fmt --all -- --check

clippy:
	cd $(RUST_DIR) && $(CARGO) clippy --locked --workspace --all-targets --all-features -- -D warnings

# parity 在 check 里:它断言 kloop 自己原生报告里的行为,改了行为只跑 test 是绿的,
# 只有它会红(教训 156)。没有语料的机器上它跳过,不拖累别处。
check: fmt-check clippy test parity

# Claude Code 2.1.220 的 parity 语料。语料不在版本控制里(见 .gitignore),
# 只在当初生成它的机器上;别处跑 make parity 会跳过而不是报错。
parity:
	@if [ -f refs/claude-code-2.1.220/verify.py ]; then \
	  python3 -B refs/claude-code-2.1.220/verify.py --corpus-only; \
	else \
	  echo "skipped: refs/claude-code-2.1.220/ is not on this machine"; \
	fi

# 无 key 的冒烟:脚本化 Mock provider 跑通五条主线
mock:
	cd $(RUST_DIR) && $(CARGO) run --locked -p $(BIN) -- --mock

install: build install-config
	install -d "$(BINDIR)"
	install -m 755 "$(RELEASE_BIN)" "$(BINDIR)/$(BIN).new"
	mv -f "$(BINDIR)/$(BIN).new" "$(BINDIR)/$(BIN)"
	@echo "installed  $(BINDIR)/$(BIN)"
	@case ":$$PATH:" in \
	  *":$(BINDIR):"*) ;; \
	  *) echo "note: $(BINDIR) is not on PATH — add it to your shell profile";; \
	esac

# 已有配置就一个字节都不碰:里面是这台机器的网关和凭据,不是我们能重放的东西。
install-config:
	@if [ -f "$(CONFIG)" ]; then \
	  echo "kept       $(CONFIG) (already there, left untouched)"; \
	else \
	  mkdir -p "$(KLOOP_DIR)" && chmod 700 "$(KLOOP_DIR)" && \
	  cp "$(CONFIG_DEMO)" "$(CONFIG)" && chmod 600 "$(CONFIG)" && \
	  echo "copied     $(CONFIG_DEMO) -> $(CONFIG)"; \
	  echo; \
	  echo "  >> EDIT IT BEFORE THE FIRST RUN: $(CONFIG)"; \
	  echo "     every auth_header in the demo says REPLACE-ME, and the"; \
	  echo "     base_url/model are example values. kloop has no fallback —"; \
	  echo "     that file is the whole of what a run talks to."; \
	  echo; \
	fi

# 只摘二进制。配置和会话是用户的,留在 $(KLOOP_DIR)。
uninstall:
	rm -f "$(BINDIR)/$(BIN)"
	@echo "removed    $(BINDIR)/$(BIN)  (left $(KLOOP_DIR) alone)"

clean:
	cd $(RUST_DIR) && $(CARGO) clean

help:
	@echo "targets:"
	@echo "  build (default)  release 构建 -> $(RELEASE_BIN)"
	@echo "  debug            debug 构建"
	@echo "  test             cargo test --workspace"
	@echo "  fmt / fmt-check  rustfmt 写入 / 只检查"
	@echo "  clippy           clippy --all-targets --all-features -D warnings"
	@echo "  check            fmt-check + clippy + test + parity"
	@echo "  parity           Claude Code 语料校验(只在有语料的机器上)"
	@echo "  mock             cargo run -- --mock,无 key 冒烟"
	@echo "  install          装到 $(BINDIR)/$(BIN);缺配置时铺一份 $(CONFIG)"
	@echo "  uninstall        删掉 $(BINDIR)/$(BIN),不动 $(KLOOP_DIR)"
	@echo "  clean            cargo clean"
	@echo
	@echo "overrides: PREFIX=$(PREFIX)  BINDIR=$(BINDIR)  KLOOP_DIR=$(KLOOP_DIR)"

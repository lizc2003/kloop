# kloop — build / test / install
#
# 仓库根不是 cargo workspace 根,workspace 在 rust/ 下,所以每个目标都先 cd 进去。
# 覆盖点:PREFIX / BINDIR(装到哪)、KLOOP_DIR(私有状态根,配置就在它下面)。
#
#   make                 release 构建
#   make test            全量测试
#   make check           fmt --check + clippy -D warnings + test + parity(提交前的门禁,本地唯一一道)
#   make check-release   fmt --check + release clippy + release 全量测试 + parity(按需手动运行)
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
.PHONY: all build debug test test-release seatbelt-nesting fmt fmt-check clippy clippy-release check check-release check-windows parity mock install install-config uninstall clean help

all: build

build:
	cd $(RUST_DIR) && $(CARGO) build --locked --release

debug:
	cd $(RUST_DIR) && $(CARGO) build --locked

# 全量测试包含真实 seatbelt 用例，而 macOS 的沙箱不能嵌套：外层 profile 只要有一条限制，
# 内层 sandbox_apply 就报 Operation not permitted。全量测试不跳过它们（plan 283）：先试一次
# 嵌套，不行就直接失败并说明要在沙箱外运行；cargo test 带上 KLOOP_REQUIRE_SEATBELT_TESTS=1，
# 关掉测试里“在 kloop 沙箱内就跳过”的那一支。没有 sandbox-exec 的平台不检查。
seatbelt-nesting:
	@if [ -x /usr/bin/sandbox-exec ] && ! /usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true; then \
	  echo "error: the full test suite runs the real OS sandbox, which cannot nest inside another sandbox; run make test / make check outside the sandbox" >&2; \
	  exit 1; \
	fi

test: seatbelt-nesting
	cd $(RUST_DIR) && KLOOP_REQUIRE_SEATBELT_TESTS=1 $(CARGO) test --locked --workspace

test-release: seatbelt-nesting
	cd $(RUST_DIR) && KLOOP_REQUIRE_SEATBELT_TESTS=1 $(CARGO) test --locked --release --workspace

fmt:
	cd $(RUST_DIR) && $(CARGO) fmt --all

fmt-check:
	cd $(RUST_DIR) && $(CARGO) fmt --all -- --check

clippy:
	cd $(RUST_DIR) && $(CARGO) clippy --locked --workspace --all-targets --all-features -- -D warnings

clippy-release:
	cd $(RUST_DIR) && $(CARGO) clippy --locked --release --workspace --all-targets --all-features -- -D warnings

# parity 在 check 里:它断言 kloop 自己原生报告里的行为,改了行为只跑 test 是绿的,
# 只有它会红(教训 156)。没有语料的机器上它跳过,不拖累别处。
check: seatbelt-nesting fmt-check clippy test parity

check-release: seatbelt-nesting fmt-check clippy-release test-release parity

# Windows 目标的交叉检查(msvc)。按需手动运行,不属于 make check:本项目唯一的提交门禁
# 是 check,Windows 日常不校验(2026-11-04 用户定)。需要 cargo-xwin 与
# LLVM(Homebrew: brew install llvm;Rust 目标: rustup target add x86_64-pc-windows-msvc),
# 缺工具时跳过而不是报错,和 parity 一样。
check-windows:
	@if ! command -v cargo-xwin >/dev/null 2>&1; then \
	  echo "skipped: cargo-xwin is not installed (cargo install --locked cargo-xwin)"; \
	  exit 0; \
	fi; \
	if ! rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-msvc; then \
	  echo "skipped: run rustup target add x86_64-pc-windows-msvc"; \
	  exit 0; \
	fi; \
	llvm_bin="$$(brew --prefix llvm 2>/dev/null)/bin"; \
	if [ -x "$$llvm_bin/llvm-lib" ]; then export PATH="$$llvm_bin:$$PATH"; fi; \
	cd $(RUST_DIR) && $(CARGO) xwin clippy --target x86_64-pc-windows-msvc \
	  --workspace --all-targets -- -D warnings

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
	@echo "  test-release     cargo test --release --workspace"
	@echo "  fmt / fmt-check  rustfmt 写入 / 只检查"
	@echo "  clippy           clippy --all-targets --all-features -D warnings"
	@echo "  clippy-release   clippy --release --all-targets --all-features -D warnings"
	@echo "  check            fmt-check + clippy + test + parity"
	@echo "  check-release    fmt-check + clippy-release + test-release + parity(手动运行)"
	@echo "  check-windows    Windows 目标 clippy -D warnings(手动运行,需 cargo-xwin + LLVM)"
	@echo "  parity           Claude Code 语料校验(只在有语料的机器上)"
	@echo "  mock             cargo run -- --mock,无 key 冒烟"
	@echo "  install          装到 $(BINDIR)/$(BIN);缺配置时铺一份 $(CONFIG)"
	@echo "  uninstall        删掉 $(BINDIR)/$(BIN),不动 $(KLOOP_DIR)"
	@echo "  clean            cargo clean"
	@echo
	@echo "overrides: PREFIX=$(PREFIX)  BINDIR=$(BINDIR)  KLOOP_DIR=$(KLOOP_DIR)"
